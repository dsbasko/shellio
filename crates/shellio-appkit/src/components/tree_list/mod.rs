//! `TreeList` — дерево на `NSOutlineView` (ADR-0002): узлы с устойчивыми id, раскрытие и выбор за
//! узлом, собственный треугольник раскрытия на токене, Up, Down, Left и Right, намерения `select`,
//! `toggle` и `activate`.
//!
//! Чистая часть — модель с разницей состояний ([`model`]); её тестирует `make check`. Вид держит
//! один объект Objective-C на id узла (P0.2, инвариант 7): детей держит родитель, источник держит
//! корни, поэтому раскрытие и выбор переживают перезагрузку детей.

pub(crate) mod model;
mod view;

pub use view::{TreeList, TreeListFacts, TreeRowFacts};
