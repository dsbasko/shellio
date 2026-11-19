//! Сценарии стенда пробы. `highlight` идёт без окна и без `NSApplication`. Стенд с окном, событиями
//! и отметкой отрисовки — задача 6 плана, сценарии вида — с задачи 7.

// Часть стенда задачи 6 нужна сценариям задач 7–16: ожидание покрытия, синтетические клавиши и
// мышь, снимки окна, CPU простоя, наблюдение показов поверхностей. Разрешение снимается, когда их
// подключат.
pub mod ax;
pub mod ax_client;
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
pub mod frames_run;
pub mod highlight;
pub mod idle;
pub mod idle_run;
pub mod keys;
pub mod keys_run;
pub mod marker;
pub mod memory;
pub mod memory_run;
pub mod neighbor;
pub mod open;
pub mod open_smoke;
#[allow(dead_code)]
pub mod sampler;
pub mod scroll;
pub mod smoke;
#[allow(dead_code)]
pub mod snapshot;
pub mod stage;
pub mod stand;
#[allow(dead_code)]
pub mod watchdog;

thread_local! {
    static PAUSE: std::cell::Cell<Option<std::time::Duration>> = const { std::cell::Cell::new(None) };
}

/// Сценарий просит таймер опроса стенда не будить его `duration`: следующее срабатывание
/// переносится на конец паузы.
pub fn request_pause(duration: std::time::Duration) {
    PAUSE.with(|pause| pause.set(Some(duration)));
}

/// Запрошенная пауза, если она есть; запрос снимается.
pub fn take_pause() -> Option<std::time::Duration> {
    PAUSE.with(std::cell::Cell::take)
}

/// Код выхода ошибки исполнения, тот же, что у `main`.
#[allow(dead_code)]
pub const EXIT_EXECUTION: u8 = 3;

/// Идущий сценарий с окном: таймер стенда опрашивает его до итога.
pub trait Running {
    /// `None` — сценарий ещё идёт.
    fn poll(&self) -> Option<Result<crate::results::Report, calibrate_run::Failed>>;
}

impl Running for smoke::Smoke {
    fn poll(&self) -> Option<Result<crate::results::Report, calibrate_run::Failed>> {
        smoke::Smoke::poll(self)
    }
}

impl Running for calibrate_run::Calibration {
    fn poll(&self) -> Option<Result<crate::results::Report, calibrate_run::Failed>> {
        calibrate_run::Calibration::poll(self)
    }
}
