//! Сценарии стенда пробы. `highlight` идёт без окна и без `NSApplication`. Стенд с окном, событиями
//! и отметкой отрисовки — задача 6 плана.

// Модули стенда задачи 6 подключает сценарий `calibrate`. До него разрешение снимается вместе с
// подключением, промежуточная точка задачи 6.
#[allow(dead_code)]
pub mod calibrate;
pub mod calibrate_run;
#[allow(dead_code, unused_macros, unused_imports)]
pub mod counters;
#[allow(dead_code)]
pub mod drawmark;
#[allow(dead_code)]
pub mod frames;
pub mod highlight;
#[allow(dead_code)]
pub mod sampler;
#[allow(dead_code)]
pub mod stand;
#[allow(dead_code)]
pub mod watchdog;

/// Код выхода ошибки исполнения, тот же, что у `main`.
#[allow(dead_code)]
pub const EXIT_EXECUTION: u8 = 3;
