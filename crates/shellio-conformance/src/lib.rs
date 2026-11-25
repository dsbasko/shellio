//! Сверка с прототипом (ADR-0037, «Уточнено в `prototype-conformance` (планирование)»).
//!
//! Чистая библиотека без браузера и без AppKit: формат паспорта и его каноническая запись
//! ([`passport`]), годность паспорта ([`validity`]), сцена на общих входных данных ([`scene`]),
//! рецепт с постусловиями ([`recipe`]), реестр трёх видов и его полнота ([`registry`]), таблица
//! шаблонов ключей сверки ([`keys`]), контракт полей ([`fields`]) и раздел «Сверка с прототипом»
//! в незавершённых планах ([`plans`]).

#![forbid(unsafe_code)]

pub mod fields;
pub mod keys;
pub mod passport;
pub mod plans;
pub mod recipe;
pub mod registry;
pub mod scene;
pub mod validity;

use std::fmt;

/// Ошибка разбора или проверки входа сверки: путь или место и причина.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub place: String,
    pub reason: String,
}

impl Error {
    pub fn new(place: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            place: place.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.place, self.reason)
    }
}

impl std::error::Error for Error {}

/// Зона окна в ключах, сцене и реестре.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Zone {
    Left,
    Right,
    Center,
    Bottom,
}

impl Zone {
    pub const ALL: [Zone; 4] = [Zone::Left, Zone::Right, Zone::Center, Zone::Bottom];

    pub fn as_str(self) -> &'static str {
        match self {
            Zone::Left => "left",
            Zone::Right => "right",
            Zone::Center => "center",
            Zone::Bottom => "bottom",
        }
    }
}

impl fmt::Display for Zone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
