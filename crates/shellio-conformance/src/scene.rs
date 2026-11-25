//! Сцена на общих входных данных (ADR-0037, «Сцена на общих входных данных») и ожидаемый состав.
//!
//! Сцена задаёт входы состояния: название воркспейса, каталог плагинов в порядке регистрации, вкладки каждой части и
//! активную панель, фокус, доступность пунктов с причинами и выбор оформления. Прототип загружает
//! её вместо стартового мира, продукт создаёт ту же сцену штатным CLI. Состав — семантические узлы
//! состояния, которые сверяются до сопоставления элементов по ключам.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{Error, Zone};

/// Кардинальность плагина: имена продукта (`shellio_plugin_api::manifest::Cardinality`), чтобы
/// сценарий сверки создавал сцену штатным CLI из того же файла.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    Multi,
    PerWorkbench,
    PerProject,
    PerWorkspace,
    PerApp,
}

/// Наименьший размер тела в точках.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

/// Запись каталога плагинов.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    pub id: String,
    pub title: String,
    /// Имя значка набора `P`.
    pub icon: String,
    pub cardinality: Cardinality,
    pub placements: Vec<Zone>,
    pub default_zone: Zone,
    pub min_size: Size,
}

/// Вкладка части.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneTab {
    pub plugin: String,
    #[serde(default)]
    pub active: bool,
}

/// Часть зоны со своими вкладками.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenePart {
    pub tabs: Vec<SceneTab>,
}

/// Видимая зона.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneZone {
    pub zone: Zone,
    pub parts: Vec<ScenePart>,
}

/// Место фокуса: зона и номер части с единицы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartRef {
    pub zone: Zone,
    pub part: usize,
}

/// Доступность пункта (меню, выбора) с причиной недоступности.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Availability {
    pub item: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Выбор оформления.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    Light,
    Dark,
    System,
}

/// Режим клавиатуры на плашке строки статуса.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusMode {
    #[default]
    Nav,
    Input,
}

/// Подсказка строки статуса: клавиши и подпись.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusHint {
    pub key: String,
    pub label: String,
}

/// Сводки строки статуса: их задаёт сцена, а не демо-мир прототипа. Чего сцена не назвала, того
/// в строке нет.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneStatus {
    /// Плашка режима клавиатуры.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<StatusMode>,
    /// Сообщение слота строки статуса.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<StatusHint>,
    /// Нагрузка приложения целиком: готовый текст.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<String>,
}

impl SceneStatus {
    /// Контролы строки статуса по порядку на экране: ключи сверки.
    pub fn controls(&self) -> Vec<String> {
        let mode = self.mode.map(|_| "status.mode");
        let load = self.load.as_ref().map(|_| "status.load");
        mode.into_iter().chain(load).map(str::to_owned).collect()
    }
}

/// Сцена.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scene {
    /// Название единственного воркспейса сцены: подпись кнопки воркспейса в заголовке.
    pub workspace: String,
    pub catalog: Vec<Plugin>,
    pub zones: Vec<SceneZone>,
    pub focus: PartRef,
    #[serde(default)]
    pub availability: Vec<Availability>,
    pub appearance: Appearance,
    /// Режим клавиатуры сцены: его цвет берут тон фокуса и линия под активной вкладкой
    /// (ADR-0027). Продукт до P1.6 знает только навигацию.
    #[serde(default)]
    pub mode: StatusMode,
    /// Сводки строки статуса.
    #[serde(default)]
    pub status: SceneStatus,
}

impl Scene {
    pub fn parse(place: &str, text: &str) -> Result<Self, Error> {
        let scene: Scene =
            serde_json::from_str(text).map_err(|e| Error::new(place, e.to_string()))?;
        scene.check().map_err(|reason| Error::new(place, reason))?;
        Ok(scene)
    }

    /// Внутренние правила сцены: каталог без повторов, размещения допустимы, вкладки ссылаются на
    /// каталог и стоят в разрешённой зоне, в каждой части ровно одна активная вкладка, фокус
    /// указывает на существующую часть.
    pub fn check(&self) -> Result<(), String> {
        if self.workspace.trim().is_empty() {
            return Err("пустое название воркспейса".to_owned());
        }
        let mut ids = BTreeSet::new();
        for plugin in &self.catalog {
            if !ids.insert(plugin.id.as_str()) {
                return Err(format!("плагин «{}» повторяется в каталоге", plugin.id));
            }
            if plugin.placements.is_empty() || !plugin.placements.contains(&plugin.default_zone) {
                return Err(format!(
                    "плагин «{}»: зона по умолчанию вне размещений",
                    plugin.id
                ));
            }
            if plugin.min_size.width == 0 || plugin.min_size.height == 0 {
                return Err(format!("плагин «{}»: пустой min_size", plugin.id));
            }
        }
        let mut zones = BTreeSet::new();
        for zone in &self.zones {
            if !zones.insert(zone.zone) {
                return Err(format!("зона {} повторяется", zone.zone));
            }
            if zone.parts.is_empty() {
                return Err(format!("видимая зона {} без частей", zone.zone));
            }
            for (i, part) in zone.parts.iter().enumerate() {
                let place = format!("зона {}, часть {}", zone.zone, i + 1);
                if !part.tabs.is_empty() && part.tabs.iter().filter(|t| t.active).count() != 1 {
                    return Err(format!("{place}: ровно одна активная вкладка"));
                }
                for tab in &part.tabs {
                    let plugin = self
                        .catalog
                        .iter()
                        .find(|p| p.id == tab.plugin)
                        .ok_or_else(|| {
                            format!("{place}: плагина «{}» нет в каталоге", tab.plugin)
                        })?;
                    if !plugin.placements.contains(&zone.zone) {
                        return Err(format!(
                            "{place}: плагин «{}» не размещается в зоне",
                            tab.plugin
                        ));
                    }
                }
            }
        }
        let focused = self
            .zones
            .iter()
            .find(|z| z.zone == self.focus.zone)
            .is_some_and(|z| (1..=z.parts.len()).contains(&self.focus.part));
        if !focused {
            return Err("фокус указывает на несуществующую часть".to_owned());
        }
        Ok(())
    }

    /// Состав сцены без шагов рецепта: тела частей с вкладками — `ready`, без вкладок — `empty`.
    pub fn composition(&self) -> Composition {
        let zones = self
            .zones
            .iter()
            .map(|zone| {
                let parts = zone
                    .parts
                    .iter()
                    .map(|part| PartComposition {
                        tabs: part.tabs.iter().map(|t| t.plugin.clone()).collect(),
                        active: part.tabs.iter().position(|t| t.active).map(|i| i + 1),
                        body: if part.tabs.is_empty() {
                            BodyMode::Empty
                        } else {
                            BodyMode::Ready
                        },
                        tools: if part.tabs.is_empty() || is_column(zone.zone, part) {
                            Vec::new()
                        } else {
                            PART_TOOLS.iter().map(|t| (*t).to_owned()).collect()
                        },
                    })
                    .collect();
                (zone.zone, parts)
            })
            .collect();
        Composition {
            zones,
            focus: Some(self.focus),
            menu: None,
            titlebar: TITLEBAR_CONTROLS.iter().map(|c| (*c).to_owned()).collect(),
            statusline: self.status.controls(),
        }
    }
}

/// Контролы заголовка по порядку на экране: ключи сверки. Третья кнопка зон — кнопка правой
/// стороны окна, что бы она ни переключала.
pub const TITLEBAR_CONTROLS: &[&str] = &[
    "chrome.window.close",
    "chrome.window.minimize",
    "chrome.window.zoom",
    "chrome.workspace",
    "chrome.search",
    "chrome.zones.left",
    "chrome.zones.bottom",
    "chrome.zones.right",
    "chrome.appearance",
];

/// Колонка навигатора: левая часть с единственной вкладкой навигатора. Она не делится
/// (ADR-0031) и рисуется без строки вкладок.
fn is_column(zone: Zone, part: &ScenePart) -> bool {
    zone == Zone::Left && part.tabs.len() == 1 && part.tabs[0].plugin == COLUMN_PLUGIN
}

/// Плагин колонки навигатора.
pub const COLUMN_PLUGIN: &str = "navigator";

/// Контролы строки вкладок части с вкладками без шагов рецепта, по порядку: сегменты `{tool}`.
pub const PART_TOOLS: &[&str] = &["split", "zoom", "menu"];

/// Режим тела части.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyMode {
    Ready,
    /// Заглушка «Развернуть» ниже `min_size`.
    Stub,
    /// Заглушка статуса панели.
    StatusStub,
    Empty,
}

/// Состав части: типы плагинов вкладок по порядку, номер активной вкладки, режим тела и
/// контролы строки вкладок по порядку (сегменты `{tool}` ключа).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartComposition {
    pub tabs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<usize>,
    pub body: BodyMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

/// Пункт открытого меню или выбора.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MenuItem {
    pub title: String,
    pub enabled: bool,
}

/// Ожидаемый состав состояния.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Composition {
    /// Видимые зоны и их части по порядку на экране.
    pub zones: BTreeMap<Zone, Vec<PartComposition>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<PartRef>,
    /// Пункты открытого меню или выбора по порядку.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu: Option<Vec<MenuItem>>,
    /// Видимые контролы заголовка по порядку: ключи сверки.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub titlebar: Vec<String>,
    /// Видимые контролы строки статуса по порядку: ключи сверки.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub statusline: Vec<String>,
}

impl Composition {
    /// Расхождения наблюдаемого состава с ожидаемым, по одному на узел.
    pub fn diff(&self, observed: &Composition) -> Vec<String> {
        let mut out = Vec::new();
        for zone in Zone::ALL {
            match (self.zones.get(&zone), observed.zones.get(&zone)) {
                (None, None) => {}
                (Some(_), None) => out.push(format!("зона {zone}: ожидается видимой, скрыта")),
                (None, Some(_)) => out.push(format!("зона {zone}: ожидается скрытой, видима")),
                (Some(want), Some(got)) => {
                    if want.len() != got.len() {
                        out.push(format!(
                            "зона {zone}: частей ожидается {}, наблюдается {}",
                            want.len(),
                            got.len()
                        ));
                        continue;
                    }
                    for (i, (w, g)) in want.iter().zip(got).enumerate() {
                        let place = format!("зона {zone}, часть {}", i + 1);
                        if w.tabs != g.tabs {
                            out.push(format!(
                                "{place}: вкладки ожидаются {:?}, наблюдаются {:?}",
                                w.tabs, g.tabs
                            ));
                        }
                        if w.active != g.active {
                            out.push(format!(
                                "{place}: активная вкладка ожидается {:?}, наблюдается {:?}",
                                w.active, g.active
                            ));
                        }
                        if w.body != g.body {
                            out.push(format!(
                                "{place}: тело ожидается {:?}, наблюдается {:?}",
                                w.body, g.body
                            ));
                        }
                        if w.tools != g.tools {
                            out.push(format!(
                                "{place}: контролы строки вкладок ожидаются {:?}, наблюдаются {:?}",
                                w.tools, g.tools
                            ));
                        }
                    }
                }
            }
        }
        if self.focus.is_some() && self.focus != observed.focus {
            out.push(format!(
                "фокус ожидается {:?}, наблюдается {:?}",
                self.focus, observed.focus
            ));
        }
        if self.menu != observed.menu {
            out.push(format!(
                "меню ожидается {:?}, наблюдается {:?}",
                self.menu, observed.menu
            ));
        }
        if self.titlebar != observed.titlebar {
            out.push(format!(
                "контролы заголовка ожидаются {:?}, наблюдаются {:?}",
                self.titlebar, observed.titlebar
            ));
        }
        if self.statusline != observed.statusline {
            out.push(format!(
                "контролы строки статуса ожидаются {:?}, наблюдаются {:?}",
                self.statusline, observed.statusline
            ));
        }
        out
    }

    /// Число частей каждой видимой зоны.
    pub fn parts(&self) -> BTreeMap<Zone, usize> {
        self.zones.iter().map(|(z, p)| (*z, p.len())).collect()
    }
}
