//! Продуктовый процесс shellio без AppKit: владелец профиля, второй запуск и аргументы запуска
//! (ADR-0006). UI подключается портом [`owner::Ui`]: бинарь `shellio` даёт его из
//! `shellio-appkit`, `shellio-owner-fixture` — без UI. Список регистраций плагинов — [`plugins`], чистая часть адаптера
//! окна — [`scene`] и [`bodies`].

#![forbid(unsafe_code)]

pub mod bodies;
pub mod gallery;
pub mod launch;
pub mod owner;
pub mod plugins;
pub mod scene;
