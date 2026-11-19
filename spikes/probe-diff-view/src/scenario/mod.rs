//! Сценарии стенда пробы. `highlight` идёт без окна и без `NSApplication`. Стенд с окном, событиями
//! и отметкой отрисовки — задача 6 плана.

// Часть стенда задачи 6 нужна сценариям задач 7–16: ожидание покрытия, синтетические клавиши и
// мышь, снимки окна, CPU простоя, наблюдение показов поверхностей. Разрешение снимается, когда их
// подключат.
pub mod calibrate;
pub mod calibrate_run;
pub mod capture;
#[allow(dead_code, unused_macros, unused_imports)]
pub mod counters;
#[allow(dead_code)]
pub mod drawmark;
#[allow(dead_code)]
pub mod events;
#[allow(dead_code)]
pub mod frames;
pub mod highlight;
pub mod marker;
pub mod neighbor;
#[allow(dead_code)]
pub mod sampler;
pub mod smoke;
#[allow(dead_code)]
pub mod snapshot;
pub mod stand;
#[allow(dead_code)]
pub mod watchdog;

/// Код выхода ошибки исполнения, тот же, что у `main`.
#[allow(dead_code)]
pub const EXIT_EXECUTION: u8 = 3;
