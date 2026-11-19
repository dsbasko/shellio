//! Сценарии стенда пробы. `highlight` идёт без окна и без `NSApplication`. Стенд с окном, событиями
//! и отметкой отрисовки — задача 6 плана, сценарии вида — с задачи 7.

pub mod accept_run;
pub mod anchor;
pub mod anchor_run;
pub mod ax;
pub mod ax_client;
pub mod bounded;
pub mod bounded_run;
pub mod calibrate;
pub mod calibrate_run;
pub mod capture;
pub mod contract;
pub mod contract_run;
pub mod counters;
pub mod drawmark;
pub mod events;
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
pub mod pairs;
pub mod pairs_run;
pub mod render;
pub mod sampler;
pub mod scroll;
pub mod selection;
pub mod selection_run;
pub mod smoke;
pub mod snapshot;
pub mod stage;
pub mod stand;
pub mod theme;
pub mod theme_run;
pub mod threads;
pub mod threads_run;
pub mod watchdog;
pub mod zones;
pub mod zones_run;

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

/// Код выхода при ошибке исполнения: стенд не дошёл до конца, предусловие не выполнено,
/// тайм-аут. Отрицательный вывод гипотезы ошибкой исполнения не считается.
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
