//! Рецепт состояния (ADR-0037, «Рецепт и постусловия»): сцена, окно, ось движения, шаги над
//! сценой и постусловия. Тему и контраст задаёт ячейка состояния; рецепт может закрепить одну из
//! осей, и тогда ячейки состояния обязаны с ней совпадать.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::passport::{Cell, Contrast, Theme};
use crate::scene::{Composition, Scene};
use crate::{Error, Zone};

/// Окно рецепта. По умолчанию 1180×720 при DPR 2.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeWindow {
    pub width: u32,
    pub height: u32,
    pub dpr: u32,
}

impl Default for RecipeWindow {
    fn default() -> Self {
        Self {
            width: 1180,
            height: 720,
            dpr: 2,
        }
    }
}

/// Ось движения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Motion {
    #[default]
    Full,
    Reduce,
}

/// Шаг над сценой.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    /// Деление части: в новой части открывается плагин.
    Split {
        zone: Zone,
        part: usize,
        plugin: String,
    },
    /// Деление, которое получает отказ `split_no_room`.
    SplitRefused {
        zone: Zone,
        part: usize,
        plugin: String,
    },
    Zoom {
        zone: Zone,
        part: usize,
    },
    HideZone {
        zone: Zone,
    },
    ShowZone {
        zone: Zone,
    },
    /// Открыть меню «…» части.
    OpenMenu {
        zone: Zone,
        part: usize,
    },
    /// Открыть выбор «Открыть в новой части».
    OpenPicker {
        zone: Zone,
        part: usize,
    },
    /// Закрыть вкладку плагина.
    CloseTab {
        zone: Zone,
        part: usize,
        plugin: String,
    },
}

/// Постусловия: ожидаемый состав и число частей каждой видимой зоны.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Postconditions {
    pub parts: BTreeMap<Zone, usize>,
    /// Ожидаемый состав после шагов. Без него — состав сцены.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<Composition>,
}

/// Рецепт.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// Имя сцены в `conformance/scenes/` без расширения.
    pub scene: String,
    #[serde(default)]
    pub window: RecipeWindow,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<Theme>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contrast: Option<Contrast>,
    #[serde(default)]
    pub motion: Motion,
    #[serde(default)]
    pub steps: Vec<Step>,
    pub post: Postconditions,
}

impl Recipe {
    pub fn parse(place: &str, text: &str) -> Result<Self, Error> {
        let recipe: Recipe =
            serde_json::from_str(text).map_err(|e| Error::new(place, e.to_string()))?;
        if recipe.window.width == 0 || recipe.window.height == 0 || recipe.window.dpr == 0 {
            return Err(Error::new(place, "пустое окно рецепта"));
        }
        Ok(recipe)
    }

    /// Ожидаемый состав рецепта над сценой. Число частей постусловия обязано совпадать с ним.
    pub fn expected(&self, scene: &Scene) -> Result<Composition, String> {
        let composition = self
            .post
            .composition
            .clone()
            .unwrap_or_else(|| scene.composition());
        if composition.parts() != self.post.parts {
            return Err(format!(
                "число частей постусловия {:?} расходится с составом {:?}",
                self.post.parts,
                composition.parts()
            ));
        }
        Ok(composition)
    }

    /// Проверка постусловий по наблюдаемому составу: пустой список — выполнены.
    pub fn check_post(&self, scene: &Scene, observed: &Composition) -> Vec<String> {
        match self.expected(scene) {
            Ok(expected) => expected.diff(observed),
            Err(reason) => vec![reason],
        }
    }

    /// Ячейка допустима для рецепта: закреплённая рецептом ось совпадает с осью ячейки.
    pub fn allows(&self, cell: Cell) -> bool {
        self.theme.is_none_or(|t| t == cell.theme)
            && self.contrast.is_none_or(|c| c == cell.contrast)
    }
}
