//! Фабрика вида: нейтральная карточка.

use shellio_plugin_view::{NeutralFactory, ViewFactory};

use crate::model::MANIFEST;

/// Фабрика тела панели на главном потоке.
#[must_use]
pub fn factory() -> Box<dyn ViewFactory> {
    Box::new(NeutralFactory::new(MANIFEST.id, MANIFEST.title))
}
