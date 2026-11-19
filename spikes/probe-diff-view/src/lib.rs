//! Чистые модули пробы P0.6 `probe-diff-view` без AppKit: модель diff, генераторы фикстур, подсветка
//! на worker и отменяемый фоновый исполнитель. Они идут в `make check`, бинарь пробы подключает их
//! по мере задач плана.

pub mod access;
pub mod estimate;
pub mod fixtures;
pub mod highlight;
pub mod keymap;
pub mod model;
pub mod open;
pub mod real;
pub mod tokens;
pub mod worker;
