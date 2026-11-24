//! Заполнитель панели навигатора до P1.7 (ADR-0008, «Уточнено в P1.5 (планирование)»): манифест,
//! модель без данных и нейтральное тело. Настоящий навигатор заменит содержимое crate под тем же id.

#![forbid(unsafe_code)]

pub mod model;
pub mod view;

use shellio_plugin_api::Entry;

/// Чистая запись регистрации: модель и таблица вариантов.
#[must_use]
pub fn entry() -> Entry {
    Entry::new(model::Model, model::VARIANTS)
}
