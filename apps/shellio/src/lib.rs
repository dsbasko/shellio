//! Продуктовый процесс shellio без AppKit: владелец профиля, второй запуск и аргументы запуска
//! (ADR-0006). UI подключается портом [`owner::Ui`]: бинарь `shellio` даёт его из
//! `shellio-appkit`, `shellio-owner-fixture` — без UI. Список регистраций плагинов — [`plugins`], чистая часть адаптера
//! окна — [`scene`], [`bodies`] и [`receipts`], служба таблицы клавиш — [`keymap_service`], чистая часть
//! маршрутизатора клавиш — [`key_router`], перевод действий таблицы в исполнение — [`key_actions`],
//! правило точек истории переходов — [`history`].

#![forbid(unsafe_code)]

pub mod bodies;
pub mod calibration;
pub mod gallery;
pub mod history;
pub mod input_sink;
pub mod key_actions;
pub mod key_router;
pub mod keymap_service;
pub mod launch;
pub mod owner;
pub mod palette_source;
pub mod plugins;
pub mod receipts;
pub mod scene;
