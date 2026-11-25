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
    /// Фокус на активную вкладку части.
    Focus {
        zone: Zone,
        part: usize,
    },
    /// Перенос вкладки плагина в зону `to`: в её последнюю активную часть или, с `new_part`, в
    /// новую часть. Место перенос не проверяет (ADR-0009), поэтому им возникает заглушка.
    Move {
        zone: Zone,
        part: usize,
        plugin: String,
        to: Zone,
        #[serde(default)]
        new_part: bool,
    },
    /// Закрыть вкладку плагина.
    CloseTab {
        zone: Zone,
        part: usize,
        plugin: String,
    },
    /// Клавиши в записи keymap: аккорды последовательности через пробел («Ctrl-w v»). Нажатие
    /// идёт в окно с фокусом на панели сцены.
    Key {
        keys: String,
    },
    /// Правка `keymap.toml` профиля: продукт получает текст файла, прототип — те же привязки под
    /// своими именами команд (`commands`: команда прототипа и её клавиши; пустой список снимает
    /// клавиши команды). Разбора TOML у прототипа нет.
    Keymap {
        text: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        commands: BTreeMap<String, Vec<String>>,
    },
    /// Фокус в поле активной панели части: первым ответчиком становится поле.
    FocusField {
        zone: Zone,
        part: usize,
    },
    /// Текст строки фильтра панели с фокусом.
    Filter {
        text: String,
    },
}

impl Step {
    /// Имя шага на проводе.
    pub fn op(&self) -> &'static str {
        match self {
            Step::Split { .. } => "split",
            Step::SplitRefused { .. } => "split_refused",
            Step::Zoom { .. } => "zoom",
            Step::HideZone { .. } => "hide_zone",
            Step::ShowZone { .. } => "show_zone",
            Step::OpenMenu { .. } => "open_menu",
            Step::OpenPicker { .. } => "open_picker",
            Step::Focus { .. } => "focus",
            Step::Move { .. } => "move",
            Step::CloseTab { .. } => "close_tab",
            Step::Key { .. } => "key",
            Step::Keymap { .. } => "keymap",
            Step::FocusField { .. } => "focus_field",
            Step::Filter { .. } => "filter",
        }
    }

    /// Внутренние правила шага: клавиши и клавиши команд прототипа названы, строка keymap не
    /// пуста.
    fn check(&self) -> Result<(), String> {
        let chords = |keys: &str| !keys.is_empty() && keys.split(' ').all(|c| !c.is_empty());
        match self {
            Step::Key { keys } if !chords(keys) => {
                Err(format!("шаг key: клавиши «{keys}» не разобраны"))
            }
            Step::Keymap { commands, .. } => commands
                .iter()
                .flat_map(|(id, keys)| keys.iter().map(move |k| (id, k)))
                .find(|(id, k)| id.is_empty() || !chords(k))
                .map_or(Ok(()), |(id, k)| {
                    Err(format!(
                        "шаг keymap: команда «{id}», клавиши «{k}» не разобраны"
                    ))
                }),
            _ => Ok(()),
        }
    }
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
        for step in &recipe.steps {
            step.check().map_err(|reason| Error::new(place, reason))?;
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
