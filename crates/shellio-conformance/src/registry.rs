//! Реестр состояний `conformance/registry.toml` (ADR-0037, «Реестр»): строки трёх видов —
//! назначение (`[[assignment]]`), состояние (`[[state]]`) и исключение (`[[exception]]`) — и
//! правило полноты против инвентаря прототипа.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::fields::Field;
use crate::passport::Cell;
use crate::{Error, Zone};

/// Ось, которую сценарий прототипа переключает вместо отдельного состояния.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    Theme,
    Contrast,
    Motion,
}

/// Поверхность состояния: в её пределах проверяются состав, контролы и требование ключей.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    Titlebar,
    Statusline,
    Layout,
    Menu,
    Picker,
}

impl Surface {
    /// Ключ принадлежит поверхности: заголовок — `chrome` и всё под ним, строка статуса —
    /// `status` и всё под ним, остальные поверхности — всё окно.
    pub fn owns(self, key: &str) -> bool {
        let under = |root: &str| {
            key.strip_prefix(root)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(['.', ':']))
        };
        match self {
            Surface::Titlebar => under("chrome"),
            Surface::Statusline => under("status"),
            Surface::Layout | Surface::Menu | Surface::Picker => true,
        }
    }
}

/// Назначение: сценарий инвентаря или пара «плагин × зона» → шаг дорожной карты.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<Zone>,
    pub step: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub axis: Option<Axis>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipes: Vec<String>,
}

/// Цель назначения.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    Scenario(String),
    PluginZone(String, Zone),
}

impl Assignment {
    pub fn target(&self) -> Result<Target, String> {
        match (&self.scenario, &self.plugin, self.zone) {
            (Some(s), None, None) => Ok(Target::Scenario(s.clone())),
            (None, Some(p), Some(z)) => Ok(Target::PluginZone(p.clone(), z)),
            _ => Err("назначение указывает либо scenario, либо plugin вместе с zone".to_owned()),
        }
    }
}

/// Состояние: требует годный паспорт каждой ячейки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub id: String,
    pub step: String,
    pub surface: Surface,
    pub scene: String,
    pub recipe: String,
    /// Ячейки; пусто — все четыре.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cells: Vec<Cell>,
    /// Сценарии инвентаря, которые это состояние показывает.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scenarios: Vec<String>,
}

impl State {
    pub fn cells(&self) -> Vec<Cell> {
        if self.cells.is_empty() {
            Cell::ALL.to_vec()
        } else {
            self.cells.clone()
        }
    }
}

/// Режим окна, исключённый целиком.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowMode {
    Gallery,
}

/// Область исключения: вид плагина, режим тела, шаблон ключа или режим окна.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    /// Вид плагина, `*` в конце — префикс (`demo-*`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<crate::scene::BodyMode>,
    /// Шаблоны ключей (`*` — один или несколько сегментов).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<WindowMode>,
}

impl Scope {
    fn is_empty(&self) -> bool {
        self.plugin.is_none() && self.body.is_none() && self.keys.is_empty() && self.mode.is_none()
    }
}

/// Исключение: состояния, область, поля, ADR и причина.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exception {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<String>,
    #[serde(default)]
    pub scope: Scope,
    #[serde(default)]
    pub fields: Vec<Field>,
    #[serde(default)]
    pub adr: String,
    #[serde(default)]
    pub reason: String,
    /// Шаг, который снимет исключение.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

/// Реестр.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    #[serde(default, rename = "assignment")]
    pub assignments: Vec<Assignment>,
    #[serde(default, rename = "state")]
    pub states: Vec<State>,
    #[serde(default, rename = "exception")]
    pub exceptions: Vec<Exception>,
}

/// Шаг дорожной карты: `P<фаза>.<номер>`.
fn check_step(step: &str) -> Result<(), String> {
    let ok = step.strip_prefix('P').is_some_and(|rest| {
        rest.split_once('.').is_some_and(|(a, b)| {
            !a.is_empty()
                && !b.is_empty()
                && a.bytes().all(|c| c.is_ascii_digit())
                && b.bytes().all(|c| c.is_ascii_digit())
        })
    });
    if ok {
        Ok(())
    } else {
        Err(format!("шаг «{step}» не в виде P<фаза>.<номер>"))
    }
}

fn check_name(kind: &str, name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{kind} «{name}»: только строчные латинские буквы, цифры и дефис"
        ))
    }
}

impl Registry {
    pub fn parse(place: &str, text: &str) -> Result<Self, Error> {
        let registry: Registry =
            toml::from_str(text).map_err(|e| Error::new(place, e.to_string()))?;
        registry
            .check()
            .map_err(|reason| Error::new(place, reason))?;
        Ok(registry)
    }

    /// Внутренние правила реестра.
    pub fn check(&self) -> Result<(), String> {
        let mut targets = BTreeSet::new();
        for a in &self.assignments {
            check_step(&a.step)?;
            let target = a.target()?;
            if !targets.insert(target.clone()) {
                return Err(format!("назначение {target:?} повторяется"));
            }
            if a.axis.is_some() && !matches!(target, Target::Scenario(_)) {
                return Err(format!("назначение {target:?}: ось — только у сценария"));
            }
        }
        let mut ids = BTreeSet::new();
        for s in &self.states {
            check_name("состояние", &s.id)?;
            check_name("сцена", &s.scene)?;
            check_name("рецепт", &s.recipe)?;
            check_step(&s.step)?;
            if !ids.insert(s.id.as_str()) {
                return Err(format!("состояние «{}» повторяется", s.id));
            }
            let mut cells = BTreeSet::new();
            if !s.cells.iter().all(|c| cells.insert(*c)) {
                return Err(format!("состояние «{}»: ячейка повторяется", s.id));
            }
            for scenario in &s.scenarios {
                if !targets.insert(Target::Scenario(scenario.clone())) {
                    return Err(format!("сценарий «{scenario}» назначен дважды"));
                }
            }
        }
        for (i, e) in self.exceptions.iter().enumerate() {
            let place = format!("исключение {}", i + 1);
            if e.scope.is_empty() {
                return Err(format!("{place}: без области"));
            }
            if e.fields.is_empty() {
                return Err(format!("{place}: без списка полей"));
            }
            if e.adr.trim().is_empty() {
                return Err(format!("{place}: без ADR"));
            }
            if e.reason.trim().is_empty() {
                return Err(format!("{place}: без причины"));
            }
            if let Some(owner) = &e.owner {
                check_step(owner)?;
            }
            for pattern in &e.scope.keys {
                crate::keys::check_glob(pattern)?;
            }
            match e.scope.mode {
                // Галерея исключается целиком: состояний галереи в реестре нет.
                Some(WindowMode::Gallery) => {
                    if !e.states.is_empty() {
                        return Err(format!("{place}: режим окна исключается без состояний"));
                    }
                }
                None => {
                    if e.states.is_empty() {
                        return Err(format!("{place}: без состояния"));
                    }
                    for state in &e.states {
                        if !ids.contains(state.as_str()) {
                            return Err(format!("{place}: состояния «{state}» нет в реестре"));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn state(&self, id: &str) -> Option<&State> {
        self.states.iter().find(|s| s.id == id)
    }
}

/// Плагин инвентаря с размещениями.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryPlugin {
    pub id: String,
    pub placements: Vec<Zone>,
}

/// Инвентарь прототипа: id зарегистрированных сценариев и плагины с размещениями со штампом.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub prototype: String,
    pub scenarios: Vec<String>,
    pub plugins: Vec<InventoryPlugin>,
}

impl Inventory {
    pub fn parse(place: &str, text: &str) -> Result<Self, Error> {
        serde_json::from_str(text).map_err(|e| Error::new(place, e.to_string()))
    }
}

/// Шаги дорожной карты, отмеченные готовыми: строки таблиц `| ✅ | P<фаза>.<номер> | …`.
pub fn done_steps(roadmap: &str) -> BTreeSet<String> {
    roadmap
        .lines()
        .filter_map(|line| {
            let mut cells = line.trim().strip_prefix('|')?.split('|').map(str::trim);
            let (mark, id) = (cells.next()?, cells.next()?);
            (mark == "✅" && check_step(id).is_ok()).then(|| id.to_owned())
        })
        .collect()
}

/// Полнота реестра против инвентаря: каждый сценарий и каждая пара «плагин × зона» имеют
/// назначение или состояние; назначение готового шага дорожной карты опирается на состояние.
/// Правило готового шага судится по назначению, а не по шагу: назначение проходит, если оно
/// называет ось (сценарий переключает ячейку, а не задаёт состояние) или список рецептов, каждый
/// из которых — рецепт состояния реестра. Пустой список — реестр полон.
pub fn completeness(
    registry: &Registry,
    inventory: &Inventory,
    done_steps: &BTreeSet<String>,
) -> Vec<String> {
    let mut covered: BTreeSet<Target> = registry
        .assignments
        .iter()
        .filter_map(|a| a.target().ok())
        .collect();
    covered.extend(
        registry
            .states
            .iter()
            .flat_map(|s| s.scenarios.iter().cloned().map(Target::Scenario)),
    );
    let mut out = Vec::new();
    for scenario in &inventory.scenarios {
        if !covered.contains(&Target::Scenario(scenario.clone())) {
            out.push(format!("сценарий «{scenario}» без назначения и состояния"));
        }
    }
    for plugin in &inventory.plugins {
        for zone in &plugin.placements {
            if !covered.contains(&Target::PluginZone(plugin.id.clone(), *zone)) {
                out.push(format!(
                    "пара «{} × {zone}» без назначения и состояния",
                    plugin.id
                ));
            }
        }
    }
    let recipes: BTreeSet<&str> = registry.states.iter().map(|s| s.recipe.as_str()).collect();
    for a in &registry.assignments {
        if let Some(unknown) = a.recipes.iter().find(|r| !recipes.contains(r.as_str())) {
            out.push(format!(
                "назначение {:?}: рецепта «{unknown}» нет у состояний реестра",
                a.target()
            ));
        }
        if done_steps.contains(&a.step) && a.axis.is_none() && a.recipes.is_empty() {
            out.push(format!(
                "шаг {} готов, а назначение {:?} без состояния",
                a.step,
                a.target()
            ));
        }
    }
    out
}
