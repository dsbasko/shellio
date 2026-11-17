//! Поверхность libghostty для сценариев пробы (функция `surface`, ADR-0034): FFI и запуск ghostty
//! (`ghostty`), окно и вид (`view`), прогон сценария `report-policy` (`scene`) и сценарий моста
//! через attach-клиент кандидата (`bridge`). Упрощённая копия P0.3: проба от
//! `spikes/probe-ghostty-surface` не зависит.

pub mod bridge;
pub mod ghostty;
pub mod scene;
pub mod view;
