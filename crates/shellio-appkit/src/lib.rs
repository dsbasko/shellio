//! Безопасный слой shellio над AppKit (ADR-0006, ADR-0020).
//!
//! Продуктовый `unsafe` вокруг Objective-C живёт здесь, типы objc2 наружу не выходят. P1.2 даёт
//! `NSApplication` без окна приложения: [`App::run`] крутит цикл главного потока с политикой
//! `.accessory` (без Dock, но с правом стать активным), [`App::become_regular`] переводит владельца
//! в `.regular`, [`App::activate`] просит ему передний план. Второй запуск показывает служебное
//! окно за пределами всех экранов ([`App::show_service_window`]), просит передний план себе
//! ([`App::request_activation`]), узнаёт об активности событием [`Delegate::did_become_active`] и
//! передаёт её владельцу по PID ([`App::hand_off_activation_to`]) с фактами [`Transfer`]. Без
//! собственной активности он просит LaunchServices открыть бандл владельца
//! ([`App::reopen_bundle_of`], ответ [`Reopened`]). [`App::finish_terminate`] отпускает приложение
//! после порядка завершения. Другие потоки попадают на главный только через [`MainThread`] и не
//! ждут его. Окно приложения, компоненты и тема — P1.3.

#![deny(clippy::undocumented_unsafe_blocks)]

mod app;

pub use app::{App, Delegate, MainThread, Reopened, RunError, Transfer};
