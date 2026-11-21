//! Продуктовый процесс shellio без AppKit: владелец профиля, второй запуск и аргументы запуска
//! (ADR-0006). UI подключается портом [`owner::Ui`]: бинарь `shellio` даёт его из
//! `shellio-appkit`, `shellio-owner-fixture` — без UI.

#![forbid(unsafe_code)]

pub mod launch;
pub mod owner;
