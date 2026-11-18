//! Чистые модули пробы P0.6 `probe-diff-view` без AppKit: модель diff и отменяемый фоновый
//! исполнитель. Они идут в `make check`, бинарь пробы подключает их по мере задач плана.

pub mod model;
pub mod worker;
